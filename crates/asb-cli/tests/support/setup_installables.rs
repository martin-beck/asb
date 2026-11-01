// Inert project-local executables for setup tests that do not exercise installs.
use std::path::Path;

pub fn seed(root: &Path, tools_dir: &str) {
    seed_entries(
        root,
        tools_dir,
        &[
            ("pi", "pi"),
            ("codex", "codex"),
            ("cli2key", "codex-bridge"),
        ],
    );
    seed_collectors(root, tools_dir);
}

pub fn seed_collectors(root: &Path, tools_dir: &str) {
    seed_entries(
        root,
        tools_dir,
        &[
            ("mpstat", "mpstat"),
            ("iostat", "iostat"),
            ("perf", "perf"),
            ("perfetto", "tracebox"),
            ("flamegraph", "flamegraph"),
            ("bpftrace", "bpftrace"),
        ],
    );
}

fn seed_entries(root: &Path, tools_dir: &str, entries: &[(&str, &str)]) {
    for (id, executable) in entries {
        let path = root.join(tools_dir).join(id).join("bin").join(executable);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture; never execute\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
}
