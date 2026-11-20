//! Private, fixed-prefix instrumentation identity, not an authentication chain.
use crate::Error;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) const PRIVATE: &str = "/opt/asb-private";
const PREFIXES: [&str; 3] = ["/opt/asb-runtime", "/opt/asb-project", PRIVATE];
#[cfg(target_os = "linux")]
const RECEIPT: &str = ".asb-owner.json";

pub(crate) fn contains_workspace(path: &Path) -> Result<bool, Error> {
    workspace_within(path, &PREFIXES)
}

fn workspace_within(path: &Path, prefixes: &[&str]) -> Result<bool, Error> {
    let resolved = path.canonicalize().map_err(|_| {
        Error::Input("cannot resolve owned task workspace for instrumentation separation".into())
    })?;
    Ok(prefixes
        .iter()
        .any(|prefix| path.starts_with(prefix) || resolved.starts_with(prefix)))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    schema_version: u32,
    scope: String,
    path: String,
    parent_device: u64,
    parent_inode: u64,
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ownership {
    pub attempt: String,
    pub nonce: String,
    pub prefixes: [Identity; 3],
}

fn invalid() -> Error {
    Error::Input("owned ASB instrumentation prefix changed or lacks its attempt receipt".into())
}

#[cfg(target_os = "linux")]
mod linux {
    #![allow(unsafe_code)]
    use super::*;
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::MetadataExt;

    #[derive(Debug)]
    pub(super) struct Pinned {
        pub parent: File,
        pub root: File,
        pub identity: Identity,
    }

    fn open(parent: i32, name: &str, flags: i32) -> Result<File, Error> {
        let name = std::ffi::CString::new(name).map_err(|_| invalid())?;
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(invalid());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub(super) fn pin(parent: &Path, path: &str) -> Result<Pinned, Error> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut anchor = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/")
            .map_err(|_| invalid())?;
        for part in parent.components() {
            match part {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    anchor = open(
                        anchor.as_raw_fd(),
                        name.to_str().ok_or_else(invalid)?,
                        libc::O_RDONLY | libc::O_DIRECTORY,
                    )?;
                }
                _ => return Err(invalid()),
            }
        }
        if !PREFIXES.contains(&path) {
            return Err(invalid());
        }
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(invalid)?;
        let root = open(anchor.as_raw_fd(), name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let before = anchor.metadata().map_err(|_| invalid())?;
        let metadata = root.metadata().map_err(|_| invalid())?;
        Ok(Pinned {
            identity: Identity {
                schema_version: 1,
                scope: "owned-asb-instrumentation-prefix-v1".into(),
                path: path.into(),
                parent_device: before.dev(),
                parent_inode: before.ino(),
                device: metadata.dev(),
                inode: metadata.ino(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                mode: metadata.mode() & 0o7777,
            },
            parent: anchor,
            root,
        })
    }

    pub(super) fn validate(pinned: &[Pinned], owner: &Ownership) -> Result<(), Error> {
        if pinned.len() != PREFIXES.len()
            || pinned
                .iter()
                .zip(&owner.prefixes)
                .zip(PREFIXES)
                .any(|((pin, identity), path)| {
                    // Docker may remount an unchanged container root between
                    // staging and the stopped application gate. Device and
                    // inode values are mount-local, so preserve them in the
                    // receipt for diagnostics but do not require them to
                    // survive that remount. The fixed path, ownership and
                    // permissions remain the guest-side custody contract.
                    pin.identity.path != path
                        || identity.path != path
                        || pin.identity.schema_version != 1
                        || pin.identity.scope != "owned-asb-instrumentation-prefix-v1"
                        || identity.schema_version != pin.identity.schema_version
                        || identity.scope != pin.identity.scope
                        || pin.identity.uid != identity.uid
                        || pin.identity.gid != identity.gid
                        || pin.identity.mode != identity.mode
                        || identity.uid != unsafe { libc::geteuid() }
                        || identity.gid != unsafe { libc::getegid() }
                        || identity.mode != 0o700
                })
            || owner.attempt.is_empty()
            || owner.attempt.len() > 128
            || !owner
                .attempt
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || owner.nonce.len() != 32
            || !owner.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        let mut file = open(
            pinned[2].root.as_raw_fd(),
            RECEIPT,
            libc::O_RDONLY | libc::O_NONBLOCK,
        )?;
        let metadata = file.metadata().map_err(|_| invalid())?;
        if !metadata.is_file()
            || metadata.len() > 16384
            || metadata.nlink() != 1
            || metadata.uid() != owner.prefixes[2].uid
            || metadata.gid() != owner.prefixes[2].gid
            || metadata.mode() & 0o7777 != 0o600
        {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.by_ref()
            .take(16385)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid())?;
        if bytes.len() > 16384
            || serde_json::from_slice::<Ownership>(&bytes).map_err(|_| invalid())? != *owner
        {
            return Err(invalid());
        }
        // Keep both descriptors alive through preparation; numeric identities
        // alone are not custody and cannot prevent inode reuse.
        for pin in pinned {
            pin.parent.metadata().map_err(|_| invalid())?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Guard {
    #[cfg(target_os = "linux")]
    pinned: Vec<linux::Pinned>,
    #[cfg(target_os = "linux")]
    parent_path: std::path::PathBuf,
}

pub(crate) fn verify(owner: &Ownership) -> Result<Guard, Error> {
    #[cfg(target_os = "linux")]
    {
        verify_at(Path::new("/opt"), owner)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = owner;
        Err(invalid())
    }
}

#[cfg(target_os = "linux")]
fn verify_at(parent: &Path, owner: &Ownership) -> Result<Guard, Error> {
    let pinned = PREFIXES
        .iter()
        .map(|path| linux::pin(parent, path))
        .collect::<Result<Vec<_>, _>>()?;
    linux::validate(&pinned, owner)?;
    Ok(Guard {
        pinned,
        parent_path: parent.to_path_buf(),
    })
}

impl Guard {
    pub(crate) fn revalidate(&self, owner: &Ownership) -> Result<(), Error> {
        #[cfg(target_os = "linux")]
        {
            linux::validate(&self.pinned, owner)?;
            let current = PREFIXES
                .iter()
                .map(|path| linux::pin(&self.parent_path, path))
                .collect::<Result<Vec<_>, _>>()?;
            linux::validate(&current, owner)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = owner;
            Err(invalid())
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/staging-ownership")
                .join(crate::workloads::TaskIdentity::local("staging", None).attempt_id);
            fs::create_dir_all(&root).unwrap();
            for prefix in PREFIXES {
                let path = root.join(Path::new(prefix).file_name().unwrap());
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            Self(root.canonicalize().unwrap())
        }
        fn receipt_path(&self) -> std::path::PathBuf {
            self.0.join("asb-private").join(RECEIPT)
        }
        fn owner(&self) -> Ownership {
            Ownership {
                attempt: "allocated-fixture-attempt".into(),
                nonce: "a".repeat(32),
                prefixes: PREFIXES.map(|path| linux::pin(&self.0, path).unwrap().identity),
            }
        }
        fn write(&self, owner: &Ownership) {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(self.receipt_path())
                .unwrap();
            file.write_all(&serde_json::to_vec(owner).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn workspace_alias_cannot_enter_instrumentation_but_ancestor_is_allowed() {
        let fixture = Fixture::new();
        let prefix = fixture.0.join("asb-private");
        let prefixes = [prefix.to_str().unwrap()];
        let alias = fixture.0.join("workspace-alias");
        symlink(&prefix, &alias).unwrap();
        fs::create_dir(prefix.join("nested")).unwrap();
        assert!(workspace_within(&alias, &prefixes).unwrap());
        assert!(workspace_within(&alias.join("nested"), &prefixes).unwrap());
        assert!(!workspace_within(&fixture.0, &prefixes).unwrap());
        assert!(workspace_within(&fixture.0.join("missing"), &prefixes).is_err());
    }

    #[test]
    fn fixed_prefix_receipt_accepts_remounted_identity_but_rejects_changed_permissions() {
        let fixture = Fixture::new();
        let mut owner = fixture.owner();
        // A Docker remount may change only device/inode values while keeping
        // the fixed prefix, owner and mode intact.
        for prefix in &mut owner.prefixes {
            prefix.parent_device = prefix.parent_device.wrapping_add(1);
            prefix.parent_inode = prefix.parent_inode.wrapping_add(1);
            prefix.device = prefix.device.wrapping_add(1);
            prefix.inode = prefix.inode.wrapping_add(1);
        }
        fixture.write(&owner);
        let guard = verify_at(&fixture.0, &owner).unwrap();
        guard.revalidate(&owner).unwrap();
        fs::set_permissions(
            fixture.0.join("asb-private"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(guard.revalidate(&owner).is_err());
        assert!(verify_at(&fixture.0, &owner).is_err());
    }

    #[test]
    fn receipt_binds_allocated_attempt_nonce_and_private_file_kind() {
        for fault in [
            "attempt",
            "nonce",
            "scope",
            "owner",
            "mode",
            "symlink",
            "hardlink",
            "fifo",
            "oversized",
            "malformed",
        ] {
            let fixture = Fixture::new();
            let mut owner = fixture.owner();
            match fault {
                "attempt" => owner.attempt.clear(),
                "nonce" => owner.nonce = "bad".into(),
                "scope" => owner.prefixes[2].scope = "foreign".into(),
                "owner" => owner.prefixes[2].uid = owner.prefixes[2].uid.wrapping_add(1),
                "mode" => {
                    fs::set_permissions(
                        fixture.0.join("asb-private"),
                        fs::Permissions::from_mode(0o755),
                    )
                    .unwrap();
                    owner.prefixes[2].mode = 0o755;
                }
                _ => {}
            }
            fixture.write(&owner);
            let receipt = fixture.receipt_path();
            match fault {
                "symlink" | "hardlink" | "fifo" => {
                    let original = fixture.0.join("retained-receipt");
                    fs::rename(&receipt, &original).unwrap();
                    if fault == "symlink" {
                        symlink(original, &receipt).unwrap();
                    } else if fault == "hardlink" {
                        fs::hard_link(original, &receipt).unwrap();
                    } else {
                        let status = std::process::Command::new("mkfifo")
                            .arg(&receipt)
                            .status()
                            .unwrap();
                        assert!(status.success());
                    }
                }
                "oversized" => fs::write(&receipt, vec![b'x'; 16385]).unwrap(),
                "malformed" => fs::write(&receipt, b"not-json").unwrap(),
                _ => {}
            }
            assert!(verify_at(&fixture.0, &owner).is_err(), "{fault}");
        }
    }

    #[test]
    fn prefix_and_ancestor_symlinks_are_never_followed() {
        let fixture = Fixture::new();
        let owner = fixture.owner();
        fixture.write(&owner);
        let alias = fixture.0.join("alias");
        symlink(&fixture.0, &alias).unwrap();
        assert!(verify_at(&alias, &owner).is_err());
        fs::rename(
            fixture.0.join("asb-project"),
            fixture.0.join("original-project"),
        )
        .unwrap();
        symlink(
            fixture.0.join("original-project"),
            fixture.0.join("asb-project"),
        )
        .unwrap();
        assert!(verify_at(&fixture.0, &owner).is_err());
    }
}

pub(crate) fn entry(args: &[std::ffi::OsString]) -> std::process::ExitCode {
    let result = (|| {
        let [path] = args else {
            return Err(invalid());
        };
        let path = path.to_str().ok_or_else(invalid)?;
        #[cfg(target_os = "linux")]
        {
            let pinned = linux::pin(Path::new("/opt"), path)?;
            serde_json::to_writer(std::io::stdout().lock(), &pinned.identity)
                .map_err(|_| invalid())?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(invalid())
        }
    })();
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(2)
    }
}
