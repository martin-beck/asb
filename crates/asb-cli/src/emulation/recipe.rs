//! Pinned x86 VM recipe primitives. Not yet connected to benchmark dispatch.

use std::ffi::{OsStr, OsString};
use std::path::Path;

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
