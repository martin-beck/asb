//! Pinned x86 VM recipe primitives. Not yet connected to benchmark dispatch.

use sha2::{Digest, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A benchmark runs natively unless an emulated backend is explicitly chosen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Backend {
    #[default]
    Native,
    EmulatedX86,
}

/// Immutable base-image identity used by the current x86 qualification recipe.
pub const X86_IMAGE_URL: &str =
    "https://cloud-images.ubuntu.com/noble/20261017/noble-server-cloudimg-amd64.img";
pub const X86_IMAGE_SHA256: &str =
    "04e8f2461a16a44e78a370284471f29616cdd9baf1893df25807b71f985b6c94";

/// Verify the base image by streaming it; no image-sized allocation is needed.
pub fn verify_x86_image(path: &Path) -> io::Result<()> {
    verify_digest(path, X86_IMAGE_SHA256)
}

fn verify_digest(path: &Path, expected: &str) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let actual = format!("{:x}", hash.finalize());
    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VM base image checksum does not match the pinned recipe; obtain the pinned image again",
        ));
    }
    Ok(())
}

/// Create an isolated writable disk over the pinned, verified base image.
/// The caller supplies a new VM directory; existing directories are preserved.
pub fn create_x86_overlay(qemu_img: &OsStr, base: &Path, vm_dir: &Path) -> io::Result<PathBuf> {
    create_overlay(qemu_img, base, vm_dir, X86_IMAGE_SHA256)
}

fn create_overlay(
    qemu_img: &OsStr,
    base: &Path,
    vm_dir: &Path,
    digest: &str,
) -> io::Result<PathBuf> {
    if qemu_img.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "qemu-img executable is empty",
        ));
    }
    let base = base.canonicalize()?;
    verify_digest(&base, digest)?;
    std::fs::create_dir(vm_dir)?;
    let disk = vm_dir.canonicalize()?.join("disk.qcow2");
    let status = Command::new(qemu_img)
        .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
        .arg(&base)
        .arg(&disk)
        .arg("250G")
        .status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "qemu-img failed to create VM disk: {status}"
        )));
    }
    Ok(disk)
}

/// Paths supplied by the caller; no host username or project location is embedded.
pub struct VmPaths<'a> {
    pub disk: &'a Path,
    pub seed: &'a Path,
    pub console: &'a Path,
}

fn qemu_path(path: &Path) -> Result<&OsStr, &'static str> {
    let value = path.as_os_str();
    if value.is_empty() || value.to_string_lossy().contains(',') {
        return Err("QEMU recipe paths must be nonempty and must not contain commas");
    }
    Ok(value)
}

fn option_path(prefix: &str, path: &Path, suffix: &str) -> Result<OsString, &'static str> {
    let mut value = OsString::from(prefix);
    value.push(qemu_path(path)?);
    value.push(suffix);
    Ok(value)
}

/// Construct an argument vector; no command shell interprets user paths.
pub fn x86_qemu_arguments(
    paths: &VmPaths<'_>,
    ssh_port: u16,
) -> Result<Vec<OsString>, &'static str> {
    if ssh_port == 0 {
        return Err("QEMU SSH forwarding needs a nonzero local port");
    }
    let disk = option_path("file=", paths.disk, ",if=virtio,format=qcow2")?;
    let seed = option_path("file=", paths.seed, ",if=virtio,format=raw,readonly=on")?;
    let console = option_path("file:", paths.console, "")?;
    Ok(vec![
        "-machine".into(),
        "q35,accel=tcg".into(),
        "-cpu".into(),
        "max".into(),
        "-smp".into(),
        "4".into(),
        "-m".into(),
        "8192".into(),
        "-drive".into(),
        disk,
        "-drive".into(),
        seed,
        "-netdev".into(),
        format!("user,id=n0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22").into(),
        "-device".into(),
        "virtio-net-pci,netdev=n0".into(),
        "-device".into(),
        "virtio-rng-pci".into(),
        "-display".into(),
        "none".into(),
        "-serial".into(),
        console,
        "-monitor".into(),
        "none".into(),
    ])
}

/// Construct a foreground VM command while leaving process ownership and I/O
/// with the execution backend. The caller decides when to spawn and stop it.
pub fn x86_qemu_command(
    executable: &OsStr,
    paths: &VmPaths<'_>,
    ssh_port: u16,
) -> Result<Command, &'static str> {
    if executable.is_empty() {
        return Err("QEMU executable must not be empty");
    }
    let mut command = Command::new(executable);
    command.args(x86_qemu_arguments(paths, ssh_port)?);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn native_is_the_default_and_image_is_immutable() {
        assert_eq!(Backend::default(), Backend::Native);
        assert_ne!(Backend::EmulatedX86, Backend::default());
        assert!(X86_IMAGE_URL.contains("/20261017/"));
        assert_eq!(X86_IMAGE_SHA256.len(), 64);
        assert!(
            X86_IMAGE_SHA256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        );
    }

    #[test]
    fn image_digest_validation_accepts_exact_bytes_and_rejects_changes() {
        let directory = std::env::current_dir()
            .unwrap()
            .join("target/vm-recipe-fixtures");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("image-digest-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert!(
            verify_digest(
                &path,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
            .is_ok()
        );
        assert_eq!(
            verify_x86_image(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::write(&path, b"abd").unwrap();
        assert_eq!(
            verify_digest(
                &path,
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            verify_x86_image(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn verified_overlay_uses_base_and_does_not_replace_existing_vm() {
        if Command::new("qemu-img").arg("--version").output().is_err() {
            return;
        }
        let parent = std::env::current_dir()
            .unwrap()
            .join("target/vm-recipe-fixtures")
            .join(format!("overlay-{}", std::process::id()));
        std::fs::create_dir_all(&parent).unwrap();
        let base = parent.join("base.qcow2");
        assert!(
            Command::new("qemu-img")
                .args(["create", "-f", "qcow2"])
                .arg(&base)
                .arg("1M")
                .status()
                .unwrap()
                .success()
        );
        let digest = format!("{:x}", Sha256::digest(std::fs::read(&base).unwrap()));
        let vm = parent.join("guest");
        let disk = create_overlay(OsStr::new("qemu-img"), &base, &vm, &digest).unwrap();
        let info = Command::new("qemu-img")
            .args(["info", "--backing-chain", "--output=json"])
            .arg(&disk)
            .output()
            .unwrap();
        assert!(info.status.success());
        assert!(
            String::from_utf8(info.stdout)
                .unwrap()
                .contains(&base.canonicalize().unwrap().to_string_lossy().to_string())
        );
        assert_eq!(
            create_overlay(OsStr::new("qemu-img"), &base, &vm, &digest)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        let rejected = parent.join("rejected");
        assert_eq!(
            create_x86_overlay(OsStr::new("qemu-img"), &base, &rejected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            create_overlay(OsStr::new("qemu-img"), &base, &rejected, "wrong")
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!rejected.exists());
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn qemu_arguments_keep_paths_as_single_arguments_and_bind_loopback() {
        let paths = VmPaths {
            disk: Path::new("/project with spaces/disk.qcow2"),
            seed: Path::new("/p/seed.img"),
            console: Path::new("/p/console.log"),
        };
        let args = x86_qemu_arguments(&paths, 22371).unwrap();
        assert!(args.contains(&OsString::from(
            "file=/project with spaces/disk.qcow2,if=virtio,format=qcow2"
        )));
        assert!(args.contains(&OsString::from(
            "user,id=n0,hostfwd=tcp:127.0.0.1:22371-:22"
        )));
        assert!(args.contains(&OsString::from("q35,accel=tcg")));
        assert!(!args.contains(&OsString::from("-daemonize")));
        assert!(!args.contains(&OsString::from("-pidfile")));
    }

    #[test]
    fn qemu_command_preserves_executable_and_arguments_without_spawning() {
        let paths = VmPaths {
            disk: Path::new("/project with spaces/disk"),
            seed: Path::new("/project with spaces/seed"),
            console: Path::new("/project with spaces/log"),
        };
        let command =
            x86_qemu_command(OsStr::new("/opt/qemu system/bin/qemu"), &paths, 22371).unwrap();
        assert_eq!(
            command.get_program(),
            OsStr::new("/opt/qemu system/bin/qemu")
        );
        assert!(
            command
                .get_args()
                .any(|arg| arg
                    == OsStr::new("file=/project with spaces/disk,if=virtio,format=qcow2"))
        );
        assert!(x86_qemu_command(OsStr::new(""), &paths, 22371).is_err());
        assert!(x86_qemu_command(OsStr::new("qemu"), &paths, 0).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn caller_can_spawn_and_reap_the_foreground_command() {
        let paths = VmPaths {
            disk: Path::new("/p/disk"),
            seed: Path::new("/p/seed"),
            console: Path::new("/p/log"),
        };
        let mut command = x86_qemu_command(OsStr::new("/bin/true"), &paths, 22371).unwrap();
        let status = command.spawn().unwrap().wait().unwrap();
        assert!(status.success());
        let mut missing = x86_qemu_command(OsStr::new("/no/such/qemu"), &paths, 22371).unwrap();
        assert_eq!(
            missing.spawn().unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
    }

    #[test]
    fn invalid_port_and_ambiguous_qemu_paths_are_rejected() {
        let paths = VmPaths {
            disk: Path::new("/p/disk"),
            seed: Path::new("/p/seed"),
            console: Path::new("/p/log"),
        };
        assert!(x86_qemu_arguments(&paths, 0).is_err());
        let invalid = VmPaths {
            disk: Path::new("/p/a,b"),
            ..paths
        };
        assert!(x86_qemu_arguments(&invalid, 22).is_err());
    }
}
