//! Actual init bootstrap causality, with no provider or download traffic.
use super::*;

const OBSERVE: &str = r#"
import json, os, pathlib, subprocess, sys
request = json.load(sys.stdin)
base = pathlib.Path(request['root'])
real = request['python']
assert sys.version_info[:2] in [(3, 12), (3, 13)], 'this control requires supported real Python'
real_version = '%d.%d' % sys.version_info[:2]
control = base / 'control-bin'
control.mkdir()
os.symlink(real, control / ('python' + real_version))
observed = {'harness_version': real_version}
for name, search in [('bootstrap', str(base / 'fake-bin')),
                     ('supported', str(control) + os.pathsep + str(base / 'fake-bin'))]:
    root = base / name
    root.mkdir()
    marker = root / 'bootstrap-arguments'
    env = {'PATH': search, 'HOME': str(root), 'TERM': 'dumb',
           'ASB_BOOTSTRAP_RECORD': str(marker),
           'all_proxy': 'socks5h://127.0.0.1:9'}
    # This control isolates Python's own bootstrap decision from init's
    # independent preparation of other missing project tools.
    if name == 'supported':
        env['ASB_INIT_OFFLINE'] = '1'
    if 'LLVM_PROFILE_FILE' in os.environ:
        env['LLVM_PROFILE_FILE'] = os.environ['LLVM_PROFILE_FILE']
    result = subprocess.run([request['asb'], 'init'], cwd=root, env=env,
                            input=b'', capture_output=True, timeout=15)
    (root / 'init.stdout').write_bytes(result.stdout)
    (root / 'init.stderr').write_bytes(result.stderr)
    entry = {'status': result.returncode,
             'stdout': result.stdout.decode('utf-8'),
             'stderr': result.stderr.decode('utf-8')}
    if name == 'supported':
        python = root / '.asb/python/venv/bin/python'
        version = subprocess.run([str(python), '-I', '-S', '-c',
                                  'import sys; print("%d.%d" % sys.version_info[:2])'],
                                 env=env, input=b'', capture_output=True, timeout=5)
        entry['version_status'] = version.returncode
        entry['version'] = version.stdout.decode('utf-8').strip()
    observed[name] = entry
print(json.dumps(observed))
"#;

#[test]
fn init_bootstrap_attempt_precedes_proxy_fixture_phase_and_real_python_avoids_it() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let python = ["python3.12", "python3.13"]
        .into_iter()
        .find_map(|name| crate::tools::host_executable(Path::new(name)))
        .expect("offline tests require real Python 3.12 or 3.13 with venv support on PATH; provision it before testing");
    let binary = std::env::var_os("ASB_TEST_PYTHON_BOOTSTRAP_ASB")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("asb")
        });
    assert!(binary.is_absolute(), "select an absolute frozen ASB path");
    assert!(binary.is_file(), "build the current ASB binary FIRST");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/python-bootstrap-offline")
        .join(crate::workloads::TaskIdentity::local("bootstrap", None).attempt_id);
    fs::create_dir_all(root.join("fake-bin")).unwrap();
    let root = root.canonicalize().unwrap();
    for name in ["python3.12", "python3.13", "python3"] {
        let path = root.join("fake-bin").join(name);
        crate::write_test_file(&root, &path, b"#!/bin/sh\nprintf '3.14\\n'\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let curl = root.join("fake-bin/curl");
    crate::write_test_file(
        &root,
        &curl,
        b"#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$ASB_BOOTSTRAP_RECORD\"\nexit 55\n",
    )
    .unwrap();
    fs::set_permissions(curl, fs::Permissions::from_mode(0o755)).unwrap();
    let request = serde_json::json!({"root": root, "python": python, "asb": binary});
    let bytes = crate::helper_exchange::run(
        &python,
        OBSERVE,
        &serde_json::to_vec(&request).unwrap(),
        crate::helper_exchange::Policy {
            request_bytes: 64 * 1024,
            response_bytes: 256 * 1024,
            timeout: Duration::from_secs(30),
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "bounded bootstrap fixture failed: {error}; retained {}",
            root.display()
        )
    });
    let observed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    fs::write(root.join("observed.json"), &bytes).unwrap();
    assert_eq!(observed["bootstrap"]["status"], 0, "{observed}");
    assert!(
        observed["bootstrap"]["stdout"]
            .as_str()
            .unwrap()
            .contains("Warning: project Python is not ready"),
        "{observed}"
    );
    let arguments = fs::read_to_string(root.join("bootstrap/bootstrap-arguments")).unwrap();
    assert!(
        arguments
            .lines()
            .any(|argument| argument
                .starts_with("https://github.com/astral-sh/uv/releases/download/")),
        "{arguments}"
    );
    assert!(root.join("bootstrap/.asb-tools/uv/cache").is_dir());
    assert!(
        fs::read_dir(root.join("bootstrap/.asb-tools"))
            .unwrap()
            .next()
            .is_some()
    );
    assert_eq!(observed["supported"]["status"], 0, "{observed}");
    assert_eq!(observed["supported"]["version_status"], 0, "{observed}");
    assert!(
        matches!(observed["harness_version"].as_str(), Some("3.12" | "3.13")),
        "{observed}"
    );
    assert_eq!(
        observed["supported"]["version"], observed["harness_version"],
        "{observed}"
    );
    assert!(
        !observed["supported"]["stdout"]
            .as_str()
            .unwrap()
            .contains("Warning: project Python"),
        "{observed}"
    );
    assert!(!root.join("supported/bootstrap-arguments").exists());
    assert!(!root.join("supported/.asb-tools/uv").exists());
    assert!(
        fs::read_dir(root.join("supported/.asb-tools"))
            .unwrap()
            .next()
            .is_none()
    );
    // The bounded helper returned only after actual owned-tree quiescence.
    // Keep successful as well as failed output/marker evidence for diagnosis.
    println!(
        "offline Python bootstrap evidence retained at {}",
        root.display()
    );
}
