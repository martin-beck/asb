//! Explicit, narrowly scoped tracefs delegation for the Perfetto adapter.

use crate::Error;
use std::io::Write;
use std::path::Path;

pub(crate) fn delegated_ready(path: &Path) -> bool {
    [
        "trace_marker",
        "tracing_on",
        "buffer_size_kb",
        "trace_clock",
        "events/enable",
    ]
    .iter()
    .all(|name| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path.join(name))
            .is_ok()
    })
}

/// Outside `.asb` so resetting that directory cannot unlink admission coordination.
/// Held only during service admission or destructive reset, never benchmark execution.
pub(crate) fn lock_admission(root: &Path) -> Result<std::fs::File, Error> {
    lock_admission_for(root, std::time::Duration::ZERO)
}

pub(crate) fn lock_admission_for(
    root: &Path,
    timeout: std::time::Duration,
) -> Result<std::fs::File, Error> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = root.join(".asb-perfetto-admission.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| Error::Io(format!("cannot open Perfetto admission lock: {error}")))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(std::fs::TryLockError::WouldBlock) => return Err(Error::Input("project reset or Perfetto service admission is still in progress; retry after initialization or service startup completes".into())),
            Err(std::fs::TryLockError::Error(error)) => return Err(Error::Io(format!("cannot lock Perfetto admission: {error}"))),
        }
    }
    let opened = file.metadata().map_err(crate::io_error)?;
    let current = std::fs::symlink_metadata(&path).map_err(crate::io_error)?;
    if !opened.is_file()
        || opened.nlink() == 0
        || opened.ino() != current.ino()
        || opened.dev() != current.dev()
    {
        return Err(Error::Input(
            "Perfetto admission lock changed; stop project initialization before retrying".into(),
        ));
    }
    Ok(file)
}

pub(super) fn lock_for_reset(root: &Path) -> Result<Vec<std::fs::File>, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut leases = vec![lock_admission(root)?];
    for name in instance_names(root).map_err(Error::Input)? {
        let path = root.join(".asb/perfetto").join(format!("{name}.lease"));
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Error::Io(format!(
                    "cannot inspect Perfetto lease before reset: {error}"
                )));
            }
        };
        file.try_lock().map_err(|_| Error::Input("cannot reset this project while a Perfetto service or its crash cleanup still owns an instance; wait for its run to stop".into()))?;
        leases.push(file);
    }
    Ok(leases)
}

/// Stable project-specific names; unrelated projects do not share a lease.
pub(crate) fn instance_names(root: &Path) -> Result<Vec<String>, String> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("cannot identify benchmark project: {error}"))?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in root.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    Ok((0..2)
        .map(|slot| format!("asb-{hash:016x}-{slot}"))
        .collect())
}

pub(super) fn configure(
    root: &Path,
    output: &mut dyn Write,
    yes: bool,
) -> Result<Vec<String>, Error> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (root, output, yes);
        Ok(Vec::new())
    }
    #[cfg(target_os = "linux")]
    {
        let parent = Path::new("/sys/kernel/tracing/instances");
        if !parent.is_dir() {
            return Ok(Vec::new());
        }
        let metadata = crate::collectors::adapters::perfetto::required_trace_metadata();
        let paths = ConfigurePaths {
            instances_parent: parent,
            metadata_paths: &metadata,
        };
        configure_at(root, output, yes, paths, |args| {
            std::process::Command::new("sudo")
                .args(args)
                .status()
                .is_ok_and(|status| status.success())
        })
    }
}

/// Private filesystem/command inputs keep delegation policy testable without
/// exposing a tracefs override to configuration, environment variables or CLI.
#[cfg(target_os = "linux")]
struct ConfigurePaths<'a> {
    instances_parent: &'a Path,
    metadata_paths: &'a [std::path::PathBuf],
}

#[cfg(target_os = "linux")]
fn configure_at(
    root: &Path,
    output: &mut dyn Write,
    yes: bool,
    paths: ConfigurePaths<'_>,
    mut run: impl FnMut(&[&str]) -> bool,
) -> Result<Vec<String>, Error> {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    let parent = paths.instances_parent;
    if !parent.is_dir() {
        return Ok(Vec::new());
    }
    let metadata = fs::metadata("/proc/self").map_err(|e| Error::Io(e.to_string()))?;
    let uid = metadata.uid();
    let owner = format!("{uid}:{}", metadata.gid());
    let names = instance_names(root).map_err(Error::Input)?;
    let metadata = paths
        .metadata_paths
        .iter()
        .filter(|path| fs::File::open(path).is_err())
        .cloned()
        .collect::<Vec<_>>();
    let pending = names
        .iter()
        .filter(|name| !delegated_ready(&parent.join(name)))
        .collect::<Vec<_>>();
    if pending.is_empty() && metadata.is_empty() {
        return Ok(names
            .into_iter()
            .filter(|name| {
                fs::symlink_metadata(parent.join(name)).is_ok_and(|m| m.is_dir() && m.uid() == uid)
                    && delegated_ready(&parent.join(name))
            })
            .collect());
    }
    let traverse = fs::metadata(parent).is_ok_and(|m| m.mode() & 1 == 0);
    writeln!(output, "Optional Perfetto setup creates or repairs two project-specific tracefs instances. Separate ASB processes lease separate instances; concurrent replicas share a service. The following commands change only new instances or this user's existing instances, plus execute-only traversal of the instances parent if needed (not write/list permission). Existing foreign-owned instances will not be adopted:").map_err(crate::io_error)?;
    if traverse {
        writeln!(output, "  sudo chmod o+x -- {}", parent.display()).map_err(crate::io_error)?;
    }
    if !metadata.is_empty() {
        writeln!(
            output,
            "  sudo chmod o+r -- {}",
            metadata
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        )
        .map_err(crate::io_error)?;
        writeln!(output, "This makes only the listed kernel event format/ID metadata readable (not event filters, raw trace data, enable controls or buffers). Tracebox needs these read-only decoder definitions even when recording an isolated instance.").map_err(crate::io_error)?;
    }
    for name in &pending {
        let path = parent.join(name);
        writeln!(
                output,
                "  sudo mkdir -- {}  (only if absent)\n  sudo chown -R {owner} -- {}\n  sudo chmod -R u+rwX -- {}",
                path.display(),
                path.display(),
                path.display()
            )
            .map_err(crate::io_error)?;
    }
    writeln!(output, "Ownership grants this user tracing control inside the listed instances. No global event/buffer/tracing_on values or sudoers will be changed; only the explicitly listed metadata read permissions and parent traversal may be widened. Instances are reusable until reboot; rerun `asb init --configure-host` after reboot. Declining leaves other collectors usable.").map_err(crate::io_error)?;
    if !crate::project::request_sudo_consent(output, yes)? {
        return Ok(Vec::new());
    }
    if traverse
        && !run(&[
            "chmod",
            "o+x",
            "--",
            parent.to_str().expect("fixed ASCII tracefs path"),
        ])
    {
        writeln!(
            output,
            "Perfetto delegation was not applied: tracefs parent traversal could not be enabled."
        )
        .map_err(crate::io_error)?;
        return Ok(Vec::new());
    }
    if !metadata.is_empty() {
        let mut args = vec!["chmod", "o+r", "--"];
        args.extend(metadata.iter().filter_map(|path| path.to_str()));
        if !run(&args) {
            writeln!(output, "Some read-only Perfetto format metadata could not be delegated; review the exact chmod command printed above.").map_err(crate::io_error)?;
        }
    }
    for name in pending {
        let path = parent.join(name);
        let path = path.to_str().expect("fixed ASCII tracefs path");
        // Never chown an already-existing foreign instance, including a
        // symlink or an old object that does not belong to this user.
        let existing = fs::symlink_metadata(path);
        let owned = match existing {
            Ok(metadata) => metadata.is_dir() && metadata.uid() == uid,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                run(&["mkdir", "--", path])
            }
            Err(_) => false,
        };
        if !owned {
            writeln!(output, "Did not adopt or modify {path}: it is foreign-owned, inaccessible, or could not be created. Review that exact instance before any manual repair.").map_err(crate::io_error)?;
            continue;
        }
        if !run(&["chown", "-R", &owner, "--", path])
            || !run(&["chmod", "-R", "u+rwX", "--", path])
            || !delegated_ready(Path::new(path))
        {
            writeln!(
                output,
                "Could not delegate {path}; repair only that instance with the printed command."
            )
            .map_err(crate::io_error)?;
        }
    }
    Ok(names
        .into_iter()
        .filter(|name| {
            fs::symlink_metadata(parent.join(name)).is_ok_and(|m| m.is_dir() && m.uid() == uid)
                && delegated_ready(&parent.join(name))
        })
        .collect())
}

#[cfg(test)]
#[path = "../../../../tests/unit/project_perfetto.rs"]
mod coverage_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn released_reset(root: &Path) -> Vec<std::fs::File> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match lock_for_reset(root) {
                Ok(guard) => return guard,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => panic!("released reset did not become available: {error}"),
            }
        }
    }
    #[test]
    #[ignore = "internal subprocess fixture invoked by the admission regression"]
    fn admission_child_fixture() {
        use std::fs::{self, OpenOptions};
        use std::time::Duration;
        let root = std::path::PathBuf::from(std::env::var_os("ASB_TEST_ADMISSION_ROOT").unwrap());
        let id = std::env::var("ASB_TEST_ADMISSION_ID").unwrap();
        let admission = lock_admission_for(&root, Duration::from_secs(3)).unwrap();
        fs::write(root.join(format!("{id}-admitting")), "ready").unwrap();
        let directory = root.join(".asb/perfetto");
        fs::create_dir_all(&directory).unwrap();
        let (slot, lease) = instance_names(&root)
            .unwrap()
            .into_iter()
            .find_map(|slot| {
                let lease = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(directory.join(format!("{slot}.lease")))
                    .unwrap();
                lease.try_lock().ok().map(|()| (slot, lease))
            })
            .expect("free instance");
        if id == "first" {
            std::thread::sleep(Duration::from_millis(250));
        }
        drop(admission);
        fs::write(root.join(format!("{id}-ready")), slot).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.join("release").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(lease);
    }

    #[test]
    fn independent_processes_wait_for_admission_then_hold_distinct_slots() {
        use std::fs;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        struct Children(Vec<std::process::Child>);
        impl Drop for Children {
            fn drop(&mut self) {
                for child in &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let root = std::env::temp_dir().join(
            crate::workloads::TaskIdentity::local("tracefs-admission-processes", None).attempt_id,
        );
        fs::create_dir_all(&root).unwrap();
        let spawn = |id| {
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "collectors::adapters::perfetto::project::tests::admission_child_fixture",
                    "--ignored",
                ])
                .env("ASB_TEST_ADMISSION_ROOT", &root)
                .env("ASB_TEST_ADMISSION_ID", id)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let wait = |name: &str| {
            let deadline = Instant::now() + Duration::from_secs(4);
            while !root.join(name).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(root.join(name).exists(), "missing fixture signal {name}");
        };
        let mut children = Children(vec![spawn("first")]);
        wait("first-admitting");
        children.0.push(spawn("second"));
        wait("first-ready");
        wait("second-ready");
        assert_ne!(
            fs::read(root.join("first-ready")).unwrap(),
            fs::read(root.join("second-ready")).unwrap()
        );
        assert!(lock_for_reset(&root).is_err());
        fs::write(root.join("release"), "stop").unwrap();
        for child in &mut children.0 {
            assert!(child.wait().unwrap().success());
        }
        drop(released_reset(&root));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_are_stable_safe_and_project_specific() {
        let root = std::env::temp_dir();
        let names = instance_names(&root).unwrap();
        assert_eq!(names, instance_names(&root.join(".")).unwrap());
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .all(|name| name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        );
        assert_ne!(
            names,
            instance_names(root.parent().unwrap_or(Path::new("/"))).unwrap()
        );
    }

    #[test]
    fn partial_delegation_is_not_treated_as_ready() {
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("tracefs-delegation", None).attempt_id);
        std::fs::create_dir_all(root.join("events")).unwrap();
        for file in [
            "trace_marker",
            "tracing_on",
            "buffer_size_kb",
            "trace_clock",
            "events/enable",
        ] {
            std::fs::write(root.join(file), "fixture").unwrap();
        }
        assert!(delegated_ready(&root));
        std::fs::remove_file(root.join("trace_marker")).unwrap();
        assert!(!delegated_ready(&root));
        std::fs::write(root.join("trace_marker"), "fixture").unwrap();
        assert!(delegated_ready(&root));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reset_cannot_remove_a_live_or_crash_cleanup_instance_lease() {
        use std::fs::{self, OpenOptions};
        let root = std::env::temp_dir()
            .join(crate::workloads::TaskIdentity::local("tracefs-reset", None).attempt_id);
        fs::create_dir_all(root.join(".asb/perfetto")).unwrap();
        let name = instance_names(&root).unwrap().remove(0);
        let path = root.join(".asb/perfetto").join(format!("{name}.lease"));
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        lease.try_lock().unwrap();
        assert!(
            lock_for_reset(&root)
                .unwrap_err()
                .to_string()
                .contains("cannot reset")
        );
        assert!(path.is_file());
        drop(lease);
        let guard = released_reset(&root);
        assert_eq!(guard.len(), 2);
        assert!(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap()
                .try_lock()
                .is_err()
        );
        drop(guard);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reset_excludes_first_admission_even_when_no_slot_files_exist() {
        use std::fs;
        let root = std::env::temp_dir().join(
            crate::workloads::TaskIdentity::local("tracefs-reset-admission", None).attempt_id,
        );
        fs::create_dir_all(root.join(".asb/perfetto")).unwrap();
        let reset = lock_for_reset(&root).unwrap();
        assert_eq!(reset.len(), 1);
        let contender = root.clone();
        assert!(
            std::thread::spawn(move || lock_admission(&contender).is_err())
                .join()
                .unwrap()
        );
        fs::remove_dir_all(root.join(".asb")).unwrap();
        fs::create_dir_all(root.join(".asb/perfetto")).unwrap();
        assert!(lock_admission(&root).is_err());
        drop(reset);
        // Parallel tests may fork with this open description immediately
        // before exec closes CLOEXEC descriptors. Allow that bounded handoff.
        let admission = lock_admission_for(&root, std::time::Duration::from_secs(1)).unwrap();
        assert!(lock_for_reset(&root).is_err());
        drop(admission);
        drop(released_reset(&root));
        fs::remove_dir_all(root).unwrap();
    }
}
