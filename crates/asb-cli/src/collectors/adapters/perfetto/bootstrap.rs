//! Instance-only Tracebox bootstrap inside an unprivileged private mount namespace.
use std::ffi::OsString;
use std::process::ExitCode;

pub(crate) fn entry(args: &[OsString]) -> ExitCode {
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Perfetto isolated bootstrap failed: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_: &[OsString]) -> Result<(), String> {
    Err("isolated Perfetto requires Linux user and mount namespaces".into())
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn run(args: &[OsString]) -> Result<(), String> {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;
    if args.len() != 6 {
        return Err("expected executable, instance trace, owner PID, lease FD and original user/mount namespace IDs".into());
    }
    let owner: i32 = args[2]
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 1)
        .ok_or("invalid owner PID")?;
    let fd: i32 = args[3]
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 2)
        .ok_or("invalid instance lease FD")?;
    // All operations occur in a normal single-threaded helper, not pre_exec.
    // Reinstall death protection after unshare's credential transitions, before
    // checking the parent race, and retain the exact PID through final exec.
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 || libc::getppid() != owner {
            return Err("owner exited during namespace bootstrap".into());
        }
        if libc::fcntl(fd, libc::F_GETFD) != 0 {
            return Err("instance lease was not retained across namespace bootstrap".into());
        }
    }
    for (namespace, expected) in ["user", "mnt"].into_iter().zip(&args[4..]) {
        let own = fs::read_link(format!("/proc/self/ns/{namespace}"))
            .map_err(|e| format!("cannot identify helper {namespace} namespace: {e}"))?;
        if own.as_os_str() == expected
            || !expected
                .to_string_lossy()
                .starts_with(&format!("{namespace}:["))
        {
            return Err(format!(
                "refusing bootstrap outside a private {namespace} namespace"
            ));
        }
    }
    let source = Path::new(&args[1]);
    let instance = source.parent().ok_or("invalid instance trace path")?;
    let name = instance
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid instance name")?;
    let root = instance
        .parent()
        .and_then(Path::parent)
        .ok_or("invalid tracefs root")?;
    if !name.starts_with("asb-")
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || instance.parent().and_then(Path::file_name) != Some(std::ffi::OsStr::new("instances"))
        || source.file_name() != Some(std::ffi::OsStr::new("trace"))
        || ![
            Path::new("/sys/kernel/tracing"),
            Path::new("/sys/kernel/debug/tracing"),
        ]
        .contains(&root)
    {
        return Err("refusing to mount anything except an ASB instance trace over its tracefs bootstrap file".into());
    }
    let source = CString::new(source.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let target =
        CString::new(root.join("trace").as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    let mut mount_info: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(source.as_ptr(), &mut mount_info) } != 0 {
        return Err(format!(
            "cannot inspect instance mount policy: {}",
            std::io::Error::last_os_error()
        ));
    }
    let readonly_flags = readonly_mount_flags(mount_info.f_flag);
    // The namespaces above are private and unprivileged. Only this process sees
    // the bind; host tracing paths, permissions and buffers are unchanged.
    for (phase, source, target, flags) in [
        (
            "private propagation",
            std::ptr::null(),
            c"/".as_ptr(),
            libc::MS_REC | libc::MS_PRIVATE,
        ),
        (
            "instance trace bind",
            source.as_ptr(),
            target.as_ptr(),
            libc::MS_BIND,
        ),
        (
            "read-only bootstrap",
            std::ptr::null(),
            target.as_ptr(),
            readonly_flags,
        ),
    ] {
        // Preserve the restrictive flags locked by an unprivileged mount
        // namespace; omitting them would attempt an unauthorized relaxation.
        if unsafe { libc::mount(source, target, std::ptr::null(), flags, std::ptr::null()) } != 0 {
            return Err(format!(
                "cannot isolate Tracebox bootstrap ({phase}): {}; check user/mount namespace availability and tracefs mount policy (rerun `asb init --configure-host` after reboot); do not grant host-wide trace access",
                std::io::Error::last_os_error()
            ));
        }
    }
    Err(format!(
        "cannot exec manual traced_probes: {}",
        Command::new(&args[0]).arg("traced_probes").exec()
    ))
}

#[cfg(target_os = "linux")]
// Linux statvfs f_flag ABI values. libc's musl bindings omit the atime
// constants even though musl exposes the same values in bits/statvfs.h.
const STATVFS_NOATIME: libc::c_ulong = 1024;
#[cfg(target_os = "linux")]
const STATVFS_NODIRATIME: libc::c_ulong = 2048;
#[cfg(target_os = "linux")]
const STATVFS_RELATIME: libc::c_ulong = 4096;

#[cfg(target_os = "linux")]
fn readonly_mount_flags(flags: libc::c_ulong) -> libc::c_ulong {
    let mut result = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY;
    for (source, target) in [
        (libc::ST_NOSUID, libc::MS_NOSUID),
        (libc::ST_NODEV, libc::MS_NODEV),
        (libc::ST_NOEXEC, libc::MS_NOEXEC),
        (STATVFS_NOATIME, libc::MS_NOATIME),
        (STATVFS_NODIRATIME, libc::MS_NODIRATIME),
        (STATVFS_RELATIME, libc::MS_RELATIME),
    ] {
        if flags & source != 0 {
            result |= target;
        }
    }
    if flags & (STATVFS_NOATIME | STATVFS_RELATIME) == 0 {
        result |= libc::MS_STRICTATIME;
    }
    result
}

#[cfg(test)]
#[path = "../../../../tests/unit/perfetto_bootstrap.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn atime_statvfs_values_match_glibc_bindings() {
        assert_eq!(super::STATVFS_NOATIME, libc::ST_NOATIME);
        assert_eq!(super::STATVFS_NODIRATIME, libc::ST_NODIRATIME);
        assert_eq!(super::STATVFS_RELATIME, libc::ST_RELATIME);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn readonly_bind_preserves_restrictive_and_actual_atime_flags() {
        let restriction = libc::ST_NOSUID | libc::ST_NODEV | libc::ST_NOEXEC;
        for (input, expected) in [
            (super::STATVFS_RELATIME, libc::MS_RELATIME),
            (
                super::STATVFS_NOATIME | super::STATVFS_NODIRATIME,
                libc::MS_NOATIME | libc::MS_NODIRATIME,
            ),
            (0, libc::MS_STRICTATIME),
        ] {
            let flags = super::readonly_mount_flags(restriction | input);
            assert_eq!(
                flags,
                libc::MS_BIND
                    | libc::MS_REMOUNT
                    | libc::MS_RDONLY
                    | libc::MS_NOSUID
                    | libc::MS_NODEV
                    | libc::MS_NOEXEC
                    | expected
            );
        }
        assert_eq!(
            super::readonly_mount_flags(0)
                & (libc::MS_NOSUID
                    | libc::MS_NODEV
                    | libc::MS_NOEXEC
                    | libc::MS_RELATIME
                    | libc::MS_NOATIME),
            0
        );
    }
    #[test]
    fn malformed_bootstrap_never_reaches_mount_or_exec() {
        assert!(super::run(&[]).is_err());
        #[cfg(target_os = "linux")]
        assert!(
            super::run(&[
                "tool".into(),
                "trace".into(),
                "0".into(),
                "0".into(),
                "user:[1]".into(),
                "mnt:[2]".into()
            ])
            .unwrap_err()
            .contains("owner PID")
        );
    }
}
