//! Observe an owned principal across a guest user-identity change.
use crate::measurements::ProcessIdentity;
use std::{io, path::PathBuf};

pub(super) fn read(identity: &ProcessIdentity) -> io::Result<PathBuf> {
    let path = format!("/proc/{}/exe", identity.pid);
    match std::fs::read_link(&path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            if !identity.matches() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "agent identity changed",
                ));
            }
            let status = std::fs::read_to_string(format!("/proc/{}/status", identity.pid))?;
            let (uid, gid) = credentials(&status)?;
            let result = with_filesystem_credentials(uid, gid, || std::fs::read_link(&path))?;
            if !identity.matches() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "agent identity changed",
                ));
            }
            Ok(result)
        }
        Err(error) => Err(error),
    }
}

fn credentials(status: &str) -> io::Result<(u32, u32)> {
    fn field(status: &str, name: &str) -> Option<u32> {
        let mut ids = status
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .split_whitespace()
            .map(str::parse::<u32>);
        let first = ids.next()?.ok()?;
        let rest = ids.collect::<Result<Vec<_>, _>>().ok()?;
        (rest.len() == 3 && rest.iter().all(|id| *id == first)).then_some(first)
    }
    match (field(status, "Uid:"), field(status, "Gid:")) {
        (Some(uid), Some(gid)) => Ok((uid, gid)),
        _ => Err(io::Error::other(
            "agent process credentials are unavailable or changing",
        )),
    }
}

// Linux filesystem credentials are per-thread. Change only those credentials,
// and restore them before returning, including when the lookup fails.
struct RestoreCredentials(u32, u32);

#[allow(unsafe_code)]
impl Drop for RestoreCredentials {
    fn drop(&mut self) {
        // SAFETY: restore the calling thread's original filesystem IDs.
        unsafe {
            libc::setfsuid(self.0);
            libc::setfsgid(self.1);
        }
    }
}

#[allow(unsafe_code)]
fn with_filesystem_credentials<T>(
    uid: u32,
    gid: u32,
    action: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    // SAFETY: setfsuid/setfsgid affect only this thread's filesystem access;
    // the sentinel queries the current ID without modifying it.
    unsafe {
        let original_uid = libc::setfsuid(u32::MAX) as u32;
        let original_gid = libc::setfsgid(u32::MAX) as u32;
        let restore = RestoreCredentials(original_uid, original_gid);
        libc::setfsgid(gid);
        libc::setfsuid(uid);
        let selected =
            libc::setfsuid(u32::MAX) as u32 == uid && libc::setfsgid(u32::MAX) as u32 == gid;
        let result = if selected {
            action()
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot observe the agent with its filesystem credentials",
            ))
        };
        drop(restore);
        if libc::setfsuid(u32::MAX) as u32 != original_uid
            || libc::setfsgid(u32::MAX) as u32 != original_gid
        {
            return Err(io::Error::other(
                "cannot restore filesystem credentials after agent observation",
            ));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_require_stable_uid_and_gid() {
        assert_eq!(
            credentials("Uid:\t1001 1001 1001 1001\nGid:\t1002 1002 1002 1002\n").unwrap(),
            (1001, 1002)
        );
        assert!(credentials("Uid:\t0 1001 1001 1001\nGid:\t1002 1002 1002 1002\n").is_err());
        assert!(credentials("Uid:\t1001\n").is_err());
    }

    #[test]
    #[allow(unsafe_code)]
    fn observes_own_live_process_and_preserves_credentials_on_error() {
        let identity = ProcessIdentity::read(std::process::id()).unwrap();
        assert!(read(&identity).unwrap().is_absolute());
        // SAFETY: querying with the sentinel leaves filesystem IDs unchanged.
        let (uid, gid) = unsafe {
            (
                libc::setfsuid(u32::MAX) as u32,
                libc::setfsgid(u32::MAX) as u32,
            )
        };
        assert!(
            with_filesystem_credentials::<()>(uid, gid, || Err(io::Error::other("fixture")))
                .is_err()
        );
        // SAFETY: querying with the sentinel leaves filesystem IDs unchanged.
        assert_eq!(
            unsafe {
                (
                    libc::setfsuid(u32::MAX) as u32,
                    libc::setfsgid(u32::MAX) as u32,
                )
            },
            (uid, gid)
        );
    }

    #[test]
    #[allow(unsafe_code)]
    fn observes_non_root_child_without_changing_the_callers_credentials() {
        use std::os::unix::process::CommandExt;
        // SAFETY: geteuid has no side effects.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("10")
            .uid(1001)
            .gid(1001)
            .spawn()
            .unwrap();
        let identity = ProcessIdentity::read(child.id()).unwrap();
        let result = read(&identity);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(result.unwrap().is_absolute());
        // SAFETY: querying with the sentinel leaves filesystem IDs unchanged.
        assert_eq!(unsafe { libc::setfsuid(u32::MAX) }, 0);
        // SAFETY: querying with the sentinel leaves filesystem IDs unchanged.
        assert_eq!(unsafe { libc::setfsgid(u32::MAX) }, 0);
    }
}
