//! Bounded observations, not a claim that all kernel or user frames resolve.
use serde_json::{Value, json};
use std::io::Read;
#[cfg(unix)]
use std::process::{Command, Stdio};

#[derive(Default, Debug, PartialEq, Eq)]
pub(super) struct Frames {
    pub total: u64,
    pub unknown: u64,
    pub kernel: u64,
    pub kernel_unknown: u64,
}

/// Inspect perf's default script output with constant memory. Only DSO labels
/// that unambiguously identify the core kernel count; unidentified DSOs may
/// still contain kernel frames and therefore never prove their absence.
pub(super) fn frames(mut input: impl Read) -> Result<Frames, String> {
    let needles: [&[u8]; 3] = [b"[unknown]", b"([kernel.kallsyms])", b"([kernel.vmlinux])"];
    let mut indices = [0; 3];
    let mut found = [false; 3];
    let mut result = Frames::default();
    let mut start = true;
    let mut frame = false;
    let mut buffer = [0u8; 8192];
    let finish = |result: &mut Frames, frame: bool, found: [bool; 3]| {
        if frame {
            result.total += 1;
            result.unknown += u64::from(found[0]);
            let kernel = found[1] || found[2];
            result.kernel += u64::from(kernel);
            result.kernel_unknown += u64::from(kernel && found[0]);
        }
    };
    loop {
        let n = input
            .read(&mut buffer)
            .map_err(|e| format!("cannot inspect decoded perf symbols: {e}"))?;
        if n == 0 {
            break;
        }
        for &b in &buffer[..n] {
            if start {
                frame = b == b'\t';
                start = false;
            }
            if b == b'\n' {
                finish(&mut result, frame, found);
                start = true;
                frame = false;
                found = [false; 3];
                indices = [0; 3];
                continue;
            }
            for i in 0..needles.len() {
                if b == needles[i][indices[i]] {
                    indices[i] += 1;
                    if indices[i] == needles[i].len() {
                        found[i] = true;
                        indices[i] = 0;
                    }
                } else {
                    indices[i] = usize::from(b == needles[i][0]);
                }
            }
        }
    }
    if !start {
        finish(&mut result, frame, found);
    }
    Ok(result)
}

/// Probe in the decoder's own privilege and namespace context. Reading a
/// nonzero address is evidence of map visibility, never of build-ID matching.
fn map_visibility(mut input: impl Read) -> &'static str {
    let mut buf = [0u8; 8192];
    match input.read(&mut buf) {
        Ok(0) | Err(_) => "unknown",
        Ok(n) => {
            let mut saw_zero = false;
            for line in buf[..n]
                .split_inclusive(|b| *b == b'\n')
                .filter(|line| line.ends_with(b"\n"))
            {
                let Some(address) = line.split(|b| *b == b' ').next() else {
                    continue;
                };
                if !address.is_empty() && address.iter().all(u8::is_ascii_hexdigit) {
                    if address.iter().any(|b| *b != b'0') {
                        return "visible";
                    }
                    saw_zero = true;
                }
            }
            if saw_zero { "zeroed" } else { "unknown" }
        }
    }
}

pub(super) fn observation(f: &Frames) -> Value {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().chars().take(256).collect::<String>());
    let restrict = std::fs::read_to_string("/proc/sys/kernel/kptr_restrict")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let maps = std::fs::File::open("/proc/kallsyms")
        .map(map_visibility)
        .unwrap_or("unknown");
    diagnostic(f, release.as_deref(), restrict, maps)
}

/// Both probes execute inside the already supervised offline decoder service.
/// Limit their output and return unknown on any failure or unexpected format.
#[cfg(unix)]
#[allow(unsafe_code)] // Only changes flags on this service's owned pipe.
fn set_nonblocking(fd: std::os::fd::RawFd) -> bool {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0
}

#[cfg(unix)]
fn buildid_output(perf: &std::ffi::OsStr, args: &[&std::ffi::OsStr]) -> Option<String> {
    use std::os::fd::AsRawFd;
    let mut child = Command::new(perf)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let fd = stdout.as_raw_fd();
    if !set_nonblocking(fd) {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut eof = false;
    let mut status = None;
    loop {
        if !eof {
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) if bytes.len() + n <= 1024 * 1024 => bytes.extend_from_slice(&buffer[..n]),
                Ok(_) => break,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(_) => break,
            }
        }
        if eof && let Some(status) = status {
            return status
                .success()
                .then(|| String::from_utf8(bytes).ok())
                .flatten();
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    None
}

#[cfg(not(unix))]
fn buildid_output(_: &std::ffi::OsStr, _: &[&std::ffi::OsStr]) -> Option<String> {
    None
}

fn valid_id(id: &str) -> bool {
    (8..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

fn recorded_id(output: &str) -> Option<String> {
    let mut ids = output.lines().filter_map(|line| {
        let mut fields = line.split_whitespace();
        let id = fields.next()?;
        let name = fields.next()?;
        (valid_id(id)
            && matches!(
                name,
                "/proc/kcore" | "[kernel.kallsyms]" | "[kernel.vmlinux]"
            ))
        .then(|| id.to_ascii_lowercase())
    });
    let first = ids.next()?;
    ids.all(|id| id == first).then_some(first)
}

fn compare_ids(recorded: Option<&str>, host: Option<&str>) -> &'static str {
    match (recorded, host) {
        (Some(a), Some(b)) if a == b => "same-kernel-build-id",
        (Some(_), Some(_)) => "different-kernel-build-id",
        _ => "unknown",
    }
}

fn debug_candidates(id: &str, home: Option<&std::ffi::OsStr>) -> &'static str {
    let mut paths = vec![
        std::path::PathBuf::from("/usr/lib/debug/.build-id")
            .join(&id[..2])
            .join(format!("{}.debug", &id[2..])),
    ];
    if let Some(home) = home {
        paths.push(
            std::path::PathBuf::from(home)
                .join(".debug/.build-id")
                .join(&id[..2])
                .join(&id[2..])
                .join("elf"),
        );
    }
    let mut inaccessible = false;
    for path in paths {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        match options.open(path) {
            Ok(file) if file.metadata().is_ok_and(|m| m.is_file()) => {
                return "readable-candidate-unverified";
            }
            Ok(_) => inaccessible = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => inaccessible = true,
        }
    }
    if inaccessible {
        "inaccessible-or-unknown"
    } else {
        "not-found-at-checked-paths"
    }
}

pub(super) fn add_build_id_evidence(
    diagnostic: &mut Value,
    perf: &std::ffi::OsStr,
    data: &std::ffi::OsStr,
) {
    let recorded = buildid_output(perf, &["buildid-list".as_ref(), "-i".as_ref(), data])
        .and_then(|s| recorded_id(&s));
    let host = buildid_output(perf, &["buildid-list".as_ref(), "-k".as_ref()]).and_then(|s| {
        let id = s.trim();
        valid_id(id).then(|| id.to_ascii_lowercase())
    });
    let comparison = compare_ids(recorded.as_deref(), host.as_deref());
    diagnostic["recorded_kernel_build_id"] = json!(recorded);
    diagnostic["decoder_host_kernel_build_id"] = json!(host);
    diagnostic["build_id_match"] = json!(comparison);
    let candidate = recorded.as_ref().map_or("unknown", |id| {
        debug_candidates(id, std::env::var_os("HOME").as_deref())
    });
    diagnostic["kernel_debug_candidate"] = json!(candidate);
    diagnostic["kernel_debug_candidate_scope"] = json!(
        "Only the system .build-id debug path and decoder HOME perf build-ID cache were checked. Other perf and symfs search paths were not checked; file presence is not verification of its build ID or use."
    );
    if comparison == "different-kernel-build-id" {
        diagnostic["build_id_guidance"] = json!(
            "The recorded core-kernel build ID differs from this decoding host's kernel. Use matching symbols for the recorded build; the current host's address map is not evidence of a match. Re-decode the retained perf.data after supplying them."
        );
    } else if comparison == "same-kernel-build-id" {
        diagnostic["build_id_guidance"] = json!(
            "The recording and decoder host report the same kernel build ID. This does not establish that a matching debug file is available or used by perf."
        );
    } else {
        diagnostic["build_id_guidance"] = json!(
            "A kernel build ID was not available for both recording and decoding host; matching remains unknown."
        );
    }
    // Matching host identity is not proof that an available debug image has
    // the same build ID, nor that perf actually used or resolved that image.
}

fn diagnostic(f: &Frames, release: Option<&str>, restrict: Option<u32>, maps: &str) -> Value {
    let state = if f.kernel_unknown > 0 {
        "kernel-frames-unresolved"
    } else if f.kernel > 0 {
        "kernel-frames-resolved"
    } else if f.total == 0 {
        "no-decoded-frames"
    } else {
        "indeterminate"
    };
    let guidance = match state {
        "kernel-frames-unresolved" => {
            "Inspect decoder permissions and matching kernel symbols/build IDs; retry decoding the retained perf.data after resolving those prerequisites."
        }
        "kernel-frames-resolved" => {
            "Identified core-kernel frames resolved. This does not establish complete kernel debug information or userspace/JIT symbol quality."
        }
        "no-decoded-frames" => {
            "The decoder returned no stack frames; kernel symbol resolution cannot be assessed."
        }
        _ => {
            "No core-kernel DSO was identified; unknown frames may include kernel frames, so their absence and resolution are not established."
        }
    };
    let maps_guidance = if maps == "zeroed" {
        "The decoder's kernel address map is zeroed. If host policy permits, an administrator can inspect kernel.kptr_restrict and temporarily run `sudo sysctl -w kernel.kptr_restrict=0`, then decode the retained perf.data again. Recording may also need suitable permissions; changing this setting exposes kernel addresses system-wide."
    } else {
        "Address map access does not itself establish matching symbols or successful decoding."
    };
    json!({"status":state,"kernel_release":release,"kptr_restrict":restrict,
        "decoder_kallsyms_addresses":maps,"identified_core_kernel_frames":f.kernel,
        "identified_unknown_core_kernel_frames":f.kernel_unknown,
        "build_id_match":"unknown", "kernel_debug_information":"unknown", "guidance":guidance, "address_map_guidance": maps_guidance,
        "limitations":"Address-map visibility and file presence alone do not prove a matching build ID or resolution; unknown frames can also be userspace or JIT code. The observation is from the decoding host, not proof of a recording host identity."})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifies_only_explicit_kernel_frames() {
        let f=frames(&b"sample\n\t1 schedule ([kernel.kallsyms])\n\t2 [unknown] ([kernel.kallsyms])\n\t3 [unknown] ([unknown])\n"[..]).unwrap();
        assert_eq!(
            f,
            Frames {
                total: 3,
                unknown: 2,
                kernel: 2,
                kernel_unknown: 1
            }
        );
        assert_eq!(
            diagnostic(&f, Some("test"), Some(1), "zeroed")["status"],
            "kernel-frames-unresolved"
        );
        assert_eq!(
            diagnostic(
                &Frames {
                    total: 1,
                    ..Frames::default()
                },
                None,
                None,
                "unknown"
            )["status"],
            "indeterminate"
        );
        assert_eq!(
            diagnostic(&Frames::default(), None, None, "unknown")["status"],
            "no-decoded-frames"
        );
        assert_eq!(
            diagnostic(
                &Frames {
                    total: 1,
                    kernel: 1,
                    ..Frames::default()
                },
                None,
                None,
                "visible"
            )["status"],
            "kernel-frames-resolved"
        );
    }
    #[test]
    fn build_ids_require_unambiguous_core_kernel_records() {
        assert_eq!(
            recorded_id("abcdef123456 /proc/kcore\n"),
            Some("abcdef123456".into())
        );
        assert_eq!(recorded_id("abcdef123456 /usr/bin/a\n"), None);
        assert_eq!(
            recorded_id("abcdef123456 /proc/kcore\n123456789abc [kernel.kallsyms]\n"),
            None
        );
        assert_eq!(recorded_id("bad /proc/kcore\n"), None);
        assert_eq!(
            compare_ids(Some("abc"), Some("abc")),
            "same-kernel-build-id"
        );
        assert_eq!(
            compare_ids(Some("abc"), Some("def")),
            "different-kernel-build-id"
        );
        assert_eq!(compare_ids(Some("abc"), None), "unknown");
    }
    #[cfg(unix)]
    #[test]
    fn build_id_probe_timeout_is_nonfatal_and_reaps_its_child() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        std::fs::create_dir(&root).unwrap();
        let program = root.join("perf-stall");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        assert!(buildid_output(program.as_os_str(), &[]).is_none());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn reports_a_build_id_cache_candidate_without_claiming_it_matches() {
        let root = std::env::temp_dir().join(crate::tools::runtime::unique_id());
        let home = root.as_os_str();
        let id = "0123456789abcdef";
        assert_eq!(
            debug_candidates(id, Some(home)),
            "not-found-at-checked-paths"
        );
        let candidate = root.join(".debug/.build-id/01/23456789abcdef/elf");
        std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        std::fs::write(candidate, b"not necessarily an ELF").unwrap();
        assert_eq!(
            debug_candidates(id, Some(home)),
            "readable-candidate-unverified"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bounded_map_probe_never_infers_symbols_from_zeroed_or_empty_maps() {
        assert_eq!(map_visibility(&b"0000000000000000 T f\n"[..]), "zeroed");
        assert_eq!(map_visibility(&b"ffffffff00000001 T f\n"[..]), "visible");
        assert_eq!(map_visibility(&b""[..]), "unknown");
        assert_eq!(
            map_visibility(&b"0000 T zero\nffff T visible\n"[..]),
            "visible"
        );
        assert_eq!(map_visibility(&b"0000 T partial"[..]), "unknown");
    }

    #[test]
    #[ignore = "requires a locally captured perf script output"]
    fn inspect_real_perf_script() {
        let path = std::env::var_os("ASB_TEST_PERF_SCRIPT").expect("ASB_TEST_PERF_SCRIPT");
        let data = frames(std::fs::File::open(path).unwrap()).unwrap();
        let expected = std::env::var("ASB_TEST_PERF_EXPECT").unwrap();
        let value = observation(&data);
        println!("{}", value);
        assert!(data.total > 0);
        assert_eq!(value["status"], expected);
    }

    #[test]
    #[ignore = "requires a local perf recording containing the host kernel build ID"]
    fn inspect_real_recorded_build_id() {
        let data = std::env::var_os("ASB_TEST_PERF_DATA").expect("ASB_TEST_PERF_DATA");
        let mut value = observation(&Frames::default());
        add_build_id_evidence(&mut value, std::ffi::OsStr::new("perf"), &data);
        println!("{value}");
        assert_eq!(value["build_id_match"], "same-kernel-build-id");
    }
}
