// Inert project-local executables for setup tests that do not exercise installs.
use std::path::Path;

pub fn seed(root: &Path) {
    for (id, executable) in [
        ("pi", "pi"),
        ("cli2key", "codex-bridge"),
        ("mpstat", "mpstat"),
        ("iostat", "iostat"),
        ("perf", "perf"),
        ("perfetto", "tracebox"),
        ("flamegraph", "flamegraph"),
        ("bpftrace", "bpftrace"),
    ] {
        let path = root
            .join(".asb-tools")
            .join(id)
            .join("bin")
            .join(executable);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture; never execute\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}
